use super::*;
use crate::daemon::status::{
    StatusPublishOutcome, StatusPublishPayload, StatusPublishRequest,
    hosted_status_publisher_with_operations,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn hosted_side_lanes_reject_payloads_outside_the_runner_scope() {
    let args = SyncOnceArgs {
        root: PathBuf::from("/tmp/scope-root"),
        state_root: PathBuf::from("/tmp/scope-state"),
        workspace_id: "workspace_expected".to_string(),
        device_id: "device_expected".to_string(),
        sync_claim: None,
        scan_scope: ScanScope::Full(FullScanReason::Startup),
    };

    assert!(
        validate_hosted_operation_scope(
            &args,
            &WorkspaceId::new("workspace_expected"),
            &DeviceId::new("device_expected"),
        )
        .is_ok()
    );
    assert!(matches!(
        validate_hosted_operation_scope(
            &args,
            &WorkspaceId::new("workspace_other"),
            &DeviceId::new("device_expected"),
        ),
        Err(SyncOnceError::InvalidOperationPayload(_))
    ));
    assert!(matches!(
        validate_hosted_operation_scope(
            &args,
            &WorkspaceId::new("workspace_expected"),
            &DeviceId::new("device_other"),
        ),
        Err(SyncOnceError::InvalidOperationPayload(_))
    ));
}

#[test]
fn hosted_sync_checks_local_workspace_key_before_resolving_hosted_context() {
    let resolver_called = Arc::new(AtomicBool::new(false));
    let resolver_called_for_call = resolver_called.clone();
    let resolver: HostedContextResolver = Arc::new(move |_| {
        resolver_called_for_call.store(true, Ordering::SeqCst);
        Err("hosted context should not be resolved".into())
    });
    let operation: HostedSyncOperation = Arc::new(|_, _, _, _| {
        panic!("sync operation should not run without its local workspace key")
    });
    let mut sync = hosted_sync_executor_with_operations(
        resolver,
        Arc::new(|_| Err(SyncOnceError::WorkspaceKeyMissing)),
        operation,
    );

    let result = sync(
        SyncOnceArgs {
            root: PathBuf::from("/tmp/prerequisite-root"),
            state_root: PathBuf::from("/tmp/prerequisite-state"),
            workspace_id: "workspace_test".to_string(),
            device_id: "device_test".to_string(),
            sync_claim: None,
            scan_scope: ScanScope::Full(FullScanReason::Startup),
        },
        None,
    );
    let error = match result {
        Ok(_) => panic!("missing workspace key should stop hosted resolution"),
        Err(error) => error,
    };

    assert!(matches!(error, SyncOnceError::WorkspaceKeyMissing));
    assert!(!resolver_called.load(Ordering::SeqCst));
}

#[test]
fn healthy_observer_reconnects_at_trust_refresh_deadline() {
    let starts = Arc::new(Mutex::new(0_u64));
    let senders = Arc::new(Mutex::new(Vec::new()));
    let starts_for_factory = starts.clone();
    let senders_for_factory = senders.clone();
    let starter: RemoteRefStreamStarter = Box::new(move |_| {
        *starts_for_factory.lock().expect("observer starts") += 1;
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(None)).expect("initial healthy state");
        senders_for_factory
            .lock()
            .expect("observer senders")
            .push(sender);
        Ok(receiver.into())
    });
    let refresh_interval = Duration::from_millis(10);
    let mut observer = remote_ref_observer_with_stream_starter_and_refresh(
        starter,
        refresh_interval,
        Arc::new(OwnedThreadMetrics::default()),
    );
    let args = SyncOnceArgs {
        root: PathBuf::from("/tmp/observer-refresh-root"),
        state_root: PathBuf::from("/tmp/observer-refresh-state"),
        workspace_id: "workspace_test".to_string(),
        device_id: "device_test".to_string(),
        sync_claim: None,
        scan_scope: ScanScope::Full(FullScanReason::Startup),
    };

    assert_eq!(
        observer
            .observe(args.clone())
            .expect("first healthy stream"),
        None
    );
    std::thread::sleep(refresh_interval + Duration::from_millis(5));
    assert!(observer.observe(args.clone()).is_err());
    assert_eq!(
        observer.observe(args).expect("refreshed healthy stream"),
        None
    );
    assert_eq!(*starts.lock().expect("observer starts"), 2);
    assert_eq!(senders.lock().expect("observer senders").len(), 2);
}

#[test]
fn successful_refresh_waits_for_the_replacement_stream_initial_value() {
    let starts = Arc::new(Mutex::new(0_u64));
    let senders = Arc::new(Mutex::new(Vec::new()));
    let starts_for_factory = starts.clone();
    let senders_for_factory = senders.clone();
    let starter: RemoteRefStreamStarter = Box::new(move |_| {
        let start = {
            let mut starts = starts_for_factory.lock().expect("observer starts");
            *starts += 1;
            *starts
        };
        let (sender, receiver) = mpsc::channel();
        if start == 1 {
            sender
                .send(Ok(Some(WorkspaceRef {
                    workspace_id: WorkspaceId::new("workspace_test"),
                    version: 1,
                    snapshot_id: SnapshotId::new("snapshot_old"),
                    updated_at: ControlPlaneTimestamp { tick: 1 },
                    updated_by_device_id: Some(DeviceId::new("device_peer")),
                })))
                .expect("initial remote ref");
        }
        senders_for_factory
            .lock()
            .expect("observer senders")
            .push(sender);
        Ok(receiver.into())
    });
    let refresh_interval = Duration::from_millis(10);
    let mut observer = remote_ref_observer_with_stream_starter_and_refresh(
        starter,
        refresh_interval,
        Arc::new(OwnedThreadMetrics::default()),
    );
    let args = SyncOnceArgs {
        root: PathBuf::from("/tmp/observer-replacement-root"),
        state_root: PathBuf::from("/tmp/observer-replacement-state"),
        workspace_id: "workspace_test".to_string(),
        device_id: "device_test".to_string(),
        sync_claim: None,
        scan_scope: ScanScope::Full(FullScanReason::Startup),
    };
    let old_ref = WorkspaceRef {
        workspace_id: WorkspaceId::new("workspace_test"),
        version: 1,
        snapshot_id: SnapshotId::new("snapshot_old"),
        updated_at: ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device_peer")),
    };
    assert_eq!(
        observer
            .observe(args.clone())
            .expect("initial stream value"),
        Some(old_ref)
    );

    std::thread::sleep(refresh_interval + Duration::from_millis(5));
    let error = observer
        .observe(args.clone())
        .expect_err("replacement is still connecting");
    assert!(error.to_string().contains("observer is connecting"));
    assert_eq!(senders.lock().expect("observer senders").len(), 2);

    let replacement_ref = WorkspaceRef {
        workspace_id: WorkspaceId::new("workspace_test"),
        version: 2,
        snapshot_id: SnapshotId::new("snapshot_replacement"),
        updated_at: ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(DeviceId::new("device_peer")),
    };
    senders
        .lock()
        .expect("observer senders")
        .get(1)
        .expect("replacement stream sender")
        .send(Ok(Some(replacement_ref.clone())))
        .expect("replacement remote ref");
    assert_eq!(
        observer.observe(args).expect("replacement stream value"),
        Some(replacement_ref)
    );
}

#[test]
fn refresh_drains_queued_ref_and_backs_off_when_replacement_start_fails() {
    let starts = Arc::new(Mutex::new(0_u64));
    let sender_slot = Arc::new(Mutex::new(None));
    let starts_for_factory = starts.clone();
    let sender_for_factory = sender_slot.clone();
    let starter: RemoteRefStreamStarter = Box::new(move |_| {
        let start = {
            let mut starts = starts_for_factory.lock().expect("observer starts");
            *starts += 1;
            *starts
        };
        if start > 1 {
            return Err(runtime_error("replacement start failed"));
        }
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(None)).expect("initial state");
        *sender_for_factory.lock().expect("observer sender") = Some(sender);
        Ok(receiver.into())
    });
    let mut observer = remote_ref_observer_with_stream_starter_and_refresh(
        starter,
        Duration::ZERO,
        Arc::new(OwnedThreadMetrics::default()),
    );
    let args = SyncOnceArgs {
        root: PathBuf::from("/tmp/observer-refresh-root"),
        state_root: PathBuf::from("/tmp/observer-refresh-state"),
        workspace_id: "workspace_test".to_string(),
        device_id: "device_test".to_string(),
        sync_claim: None,
        scan_scope: ScanScope::Full(FullScanReason::Startup),
    };
    assert_eq!(
        observer.observe(args.clone()).expect("initial stream"),
        None
    );
    let queued = WorkspaceRef {
        workspace_id: WorkspaceId::new("workspace_test"),
        version: 2,
        snapshot_id: SnapshotId::new("snapshot_queued"),
        updated_at: ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(DeviceId::new("device_peer")),
    };
    sender_slot
        .lock()
        .expect("observer sender")
        .as_ref()
        .expect("old sender")
        .send(Ok(Some(queued.clone())))
        .expect("queued ref");
    assert_eq!(
        observer
            .observe(args.clone())
            .expect("queued ref survives failed refresh"),
        Some(queued.clone())
    );
    assert_eq!(
        observer.observe(args).expect("backoff retains latest"),
        Some(queued)
    );
    assert_eq!(*starts.lock().expect("observer starts"), 2);
}

#[test]
fn production_constructors_share_one_injected_context_resolver() {
    let factory = Arc::new(CountingHostedContextFactory {
        generation: AtomicUsize::new(0),
        hosted: AtomicUsize::new(0),
        runtimes: AtomicUsize::new(0),
        http: AtomicUsize::new(0),
        identity: bowline_local::device_keys::DeviceIdentity::generate(),
    });
    let resolver =
        hosted_context_resolver(Arc::new(HostedContextCache::with_factory(factory.clone())));
    let args = SyncOnceArgs {
        root: PathBuf::from("/tmp/real-path-root"),
        state_root: PathBuf::from("/tmp/real-path-state"),
        workspace_id: "workspace_test".to_string(),
        device_id: "device_test".to_string(),
        sync_claim: None,
        scan_scope: ScanScope::Full(FullScanReason::Startup),
    };
    let observed_key_epoch = Arc::new(AtomicUsize::new(0));
    let observed_key_epoch_for_sync = observed_key_epoch.clone();
    let sync_operation: HostedSyncOperation = Arc::new(move |_, _, _, key| {
        observed_key_epoch_for_sync.store(key.key_epoch as usize, Ordering::SeqCst);
        Err(SyncOnceError::WorkspaceKeyMissing)
    });
    let dispatch_operation: HostedDispatchOperation = Arc::new(|_, _| Ok(None));
    let status_operation = Arc::new(|_, _: StatusPublishPayload| {
        Ok(StatusPublishOutcome {
            fingerprint: "test".to_string(),
        })
    });
    let observer_senders = Arc::new(Mutex::new(Vec::new()));
    let observer_operation: HostedObserverOperation = Arc::new(move |resolver, args| {
        let _context = resolver(&args)?;
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(None)).expect("observer state");
        observer_senders
            .lock()
            .expect("observer senders")
            .push(sender);
        Ok(receiver)
    });
    let mut sync = hosted_sync_executor_with_operations(
        resolver.clone(),
        Arc::new(|_| {
            Ok(LocalWorkspaceKey {
                bytes: [0_u8; 32],
                key_epoch: 7,
            })
        }),
        sync_operation,
    );
    let mut dispatch =
        hosted_dispatch_claimer_with_operations(resolver.clone(), dispatch_operation);
    let status = hosted_status_publisher_with_operations(resolver.clone(), status_operation);
    let observer_refresh_interval = Duration::from_millis(10);
    let mut observer = hosted_remote_ref_observer_with_operations_and_refresh(
        resolver.clone(),
        observer_operation,
        observer_refresh_interval,
    );

    for _ in 0..10 {
        assert!(sync(args.clone(), None).is_err());
    }
    assert_eq!(observed_key_epoch.load(Ordering::SeqCst), 7);
    for _ in 0..10 {
        assert!(dispatch(args.clone()).expect("dispatch").is_none());
    }
    for _ in 0..2 {
        status
            .publish(StatusPublishPayload::from_request(StatusPublishRequest {
                args: args.clone(),
            }))
            .expect("status");
    }
    assert_eq!(observer.observe(args.clone()).expect("observer"), None);
    std::thread::sleep(observer_refresh_interval + Duration::from_millis(5));
    assert!(observer.observe(args.clone()).is_err());
    assert_eq!(
        observer.observe(args.clone()).expect("observer reconnect"),
        None
    );
    assert_eq!(factory.hosted.load(Ordering::SeqCst), 1);
    assert_eq!(factory.runtimes.load(Ordering::SeqCst), 1);
    assert_eq!(factory.http.load(Ordering::SeqCst), 1);

    factory.generation.store(1, Ordering::SeqCst);
    assert!(sync(args.clone(), None).is_err());
    assert!(dispatch(args).expect("stabilized dispatch").is_none());
    assert_eq!(factory.hosted.load(Ordering::SeqCst), 2);
    assert_eq!(factory.runtimes.load(Ordering::SeqCst), 2);
    assert_eq!(factory.http.load(Ordering::SeqCst), 2);
}
