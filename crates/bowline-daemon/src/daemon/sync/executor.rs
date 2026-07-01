use super::*;

pub(in crate::daemon) fn run_sync_once(
    args: SyncOnceArgs,
) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    run_sync_once_observed(args, None)
}

pub(in crate::daemon) fn run_sync_once_observed(
    args: SyncOnceArgs,
    observed_base_ref: Option<WorkspaceRef>,
) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    require_convex_url()?;
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or_else(|| runtime_error("workspace key is missing; approve this device or recover the workspace before daemon sync"))?;
    let workspace_key_bytes = workspace_key_bytes(&workspace_key.key_bytes)?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())?;
    let base_ref = match observed_base_ref {
        Some(workspace_ref) => workspace_ref,
        None => match control_plane.get_workspace_ref(workspace_id.as_str())? {
            Some(workspace_ref) => workspace_ref,
            None => control_plane.create_workspace_ref(workspace_id.as_str())?,
        },
    };
    let byte_store = SignedUrlByteStore::new(&control_plane, workspace_id.as_str());

    run_sync_once_with(
        args,
        &control_plane,
        &byte_store,
        base_ref,
        workspace_id,
        device_id,
        workspace_key_bytes,
    )
}

pub(in crate::daemon) fn hosted_sync_executor() -> SyncExecutor {
    Box::new(run_sync_once_observed)
}

pub(in crate::daemon) fn stream_remote_workspace_refs(
    args: SyncOnceArgs,
) -> Result<
    Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
    Box<dyn std::error::Error>,
> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("bowline-remote-ref-observer".to_string())
        .spawn(move || {
            let result = stream_remote_workspace_refs_on_thread(
                args,
                workspace_id,
                device_id,
                sender.clone(),
            );
            if let Err(error) = result {
                let _ = sender.send(Err(ControlPlaneError::Storage(error.to_string())));
            }
        })?;
    Ok(receiver)
}

pub(in crate::daemon) fn stream_remote_workspace_refs_on_thread(
    _args: SyncOnceArgs,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    sender: Sender<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    require_convex_url()?;
    let key_store = key_store()?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id)?;
    control_plane.stream_workspace_ref_updates(workspace_id.as_str(), sender)?;
    Ok(())
}

pub(in crate::daemon) fn hosted_remote_ref_observer() -> RemoteRefObserver {
    let mut receiver = None;
    let mut latest = None;
    let mut reconnect_failure_count = 0_u32;
    let mut next_reconnect_attempt = Instant::now();
    let mut last_error = None::<String>;
    Box::new(move |args| {
        if receiver.is_none() {
            let now = Instant::now();
            if now < next_reconnect_attempt {
                let reason = last_error
                    .clone()
                    .unwrap_or_else(|| "remote ref observer is reconnecting".to_string());
                return Err(runtime_error(format!(
                    "remote ref observer reconnecting after failure: {reason}"
                )));
            }
            match stream_remote_workspace_refs(args) {
                Ok(stream) => {
                    receiver = Some(stream);
                    reconnect_failure_count = 0;
                    last_error = None;
                }
                Err(error) => {
                    reconnect_failure_count = reconnect_failure_count.saturating_add(1);
                    next_reconnect_attempt =
                        now + remote_observer_reconnect_delay(reconnect_failure_count);
                    last_error = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        let mut disconnected = false;
        let mut observer_error = None;
        if let Some(receiver) = &receiver {
            loop {
                match receiver.try_recv() {
                    Ok(Ok(Some(workspace_ref))) => latest = Some(workspace_ref),
                    Ok(Ok(None)) => latest = None,
                    Ok(Err(error)) => {
                        observer_error = Some(error);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if let Some(error) = observer_error {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            last_error = Some(error.to_string());
            receiver = None;
            latest = None;
            return Err(Box::new(error));
        }
        if disconnected {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            last_error = Some("remote ref subscription disconnected".to_string());
            receiver = None;
            latest = None;
        }
        Ok(latest.clone())
    })
}

pub(in crate::daemon) fn remote_observer_reconnect_delay(failure_count: u32) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(6);
    let multiplier = 1_u64 << exponent;
    let delay_seconds = REMOTE_OBSERVER_RECONNECT_INITIAL
        .as_secs()
        .saturating_mul(multiplier)
        .min(REMOTE_OBSERVER_RECONNECT_MAX.as_secs());
    Duration::from_secs(delay_seconds)
}

pub(in crate::daemon) fn run_sync_once_with(
    args: SyncOnceArgs,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    base_ref: bowline_control_plane::WorkspaceRef,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_key_bytes: [u8; 32],
) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    let runner = SyncRunner::new_with_base_ref(
        control_plane,
        byte_store,
        SyncRunnerOptions {
            root: args.root,
            state_root: args.state_root,
            workspace_id,
            device_id,
            workspace_content_key: workspace_key_bytes,
            storage_key: StorageKey::from_bytes(workspace_key_bytes),
            key_epoch: 1,
            generated_at: current_timestamp(),
            sync_operation_id: args.sync_operation_id.clone(),
        },
        base_ref.clone(),
    );
    match runner.tick()? {
        SyncTickOutcome::NoWorkspaceRef => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::NoChanges => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::Imported(workspace_ref) => Ok(SyncOnceSummary {
            workspace_id: workspace_ref.workspace_id,
            snapshot_id: workspace_ref.snapshot_id,
            version: workspace_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::Uploaded(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => Ok(summary_from_uploaded(
                workspace_ref,
                object_manifest,
                false,
                false,
                0,
            )),
            UploadOutcome::Stale {
                stale,
                object_manifest,
            } => Ok(summary_from_uploaded(
                stale.current,
                object_manifest,
                true,
                false,
                0,
            )),
        },
        SyncTickOutcome::Merged(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => Ok(summary_from_uploaded(
                workspace_ref,
                object_manifest,
                false,
                true,
                0,
            )),
            UploadOutcome::Stale {
                stale,
                object_manifest,
            } => Ok(summary_from_uploaded(
                stale.current,
                object_manifest,
                true,
                true,
                0,
            )),
        },
        SyncTickOutcome::Conflicted(conflicts) => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: true,
            merged: false,
            conflict_count: conflicts.len(),
            conflicts: conflicts
                .into_iter()
                .map(|conflict| ConflictSummary {
                    id: conflict.id,
                    paths: conflict.paths,
                })
                .collect(),
        }),
    }
}

pub(in crate::daemon) fn summary_from_uploaded(
    workspace_ref: bowline_control_plane::WorkspaceRef,
    object_manifest: bowline_control_plane::ObjectManifestRecord,
    stale: bool,
    merged: bool,
    conflict_count: usize,
) -> SyncOnceSummary {
    SyncOnceSummary {
        workspace_id: workspace_ref.workspace_id,
        snapshot_id: workspace_ref.snapshot_id,
        version: workspace_ref.version,
        object_manifest_id: object_manifest.manifest_id,
        manifest_object_key: object_manifest.manifest_object.object_key,
        pack_object_keys: object_manifest
            .pack_objects
            .into_iter()
            .map(|object| object.object_key)
            .collect(),
        stale,
        merged,
        conflict_count,
        conflicts: Vec::new(),
    }
}
