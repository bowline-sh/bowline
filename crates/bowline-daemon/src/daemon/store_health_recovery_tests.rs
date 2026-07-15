use super::{StatusPublishPayload, StatusPublishRequest, StatusPublisher, runtime_error};
use std::fs;

fn failing_status_publisher() -> StatusPublisher {
    StatusPublisher::new(|_| Err(runtime_error("status publisher unavailable")))
}

#[test]
fn publish_failure_does_not_block_recovery() {
    let temp = super::tests::unique_temp_dir("bowline-store-health-publish-failure");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root dir");
    let runtime = super::tests::watcher_test_runtime(root, state_root, "ws_store_health_recovery");
    let publisher = failing_status_publisher();

    let failed: Option<()> = runtime.store_health.record("forced", Err("locked"));
    assert_eq!(failed, None);
    assert!(runtime.store_health.is_degraded());

    assert!(
        publisher
            .publish(StatusPublishPayload::from_request(StatusPublishRequest {
                args: runtime.options.args.clone(),
            }))
            .is_err()
    );
    assert!(runtime.store_health.is_degraded());

    runtime.record_component_states(super::sync::SyncComponentState::Ready, "ready", "ready");
    assert!(!runtime.store_health.is_degraded());

    assert!(
        publisher
            .publish(StatusPublishPayload::from_request(StatusPublishRequest {
                args: runtime.options.args.clone(),
            }))
            .is_err()
    );
    assert!(!runtime.store_health.is_degraded());

    let _ = fs::remove_dir_all(temp);
}
